//! User guide "Where it is and how to use it": data is baked into the binary.
//!
//! Opened from the system tray (the "📖 Guide" item) in a separate window.
//! This is not "What's new" (`changelog.rs`): the release log says what changed,
//! while the guide says where each feature is and how to use it.
//!
//! Texts are bilingual: a Russian reference and an English variant, chosen by the UI
//! language by `l10n::t` when building the response. Entries are edited in Russian, English
//! is kept next to it — a separate table desynchronizes with the first edit.
//!
//! ## Entry format (developer documentation)
//!
//! Sections go from new versions to old; the first section is "Basics" (empty
//! `version`), which holds base capabilities not tied to a version and
//! service UI places (tray, settings, wand menu).
//!
//! Each entry is a `GuideEntry`:
//! - `icon` — emoji category marker (the same scale as in `changelog.rs`);
//! - `title` — short feature name (up to ~60 characters);
//! - `since` — the version where the feature appeared; `None` — a "Basics" entry;
//!   the version must exist in `changelog()`, otherwise the test fails;
//! - `location` — UI path: "Cloufam window → …", "Wand menu → …",
//!   "SillyTavern → …", "Tray → …"; for automations — "Works automatically";
//! - `usage` — how to use it: what to press, what to see, what to get;
//! - `note` — conditions and caveats ("off by default", "CUDA only").
//!
//! Adjacent improvements of one feature in different versions (for example, card
//! and lorebook translation) merge into one entry with the `since` of the first
//! version and a mention of later changes. `fixes` from the changelog stay out.

use serde::{Deserialize, Serialize};

use crate::l10n;

/// One guide feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideEntry {
    /// Category emoji marker.
    pub icon: String,
    /// Short feature name.
    pub title: String,
    /// Version of appearance; `None` — a "Basics" section entry.
    pub since: Option<String>,
    /// Where it is in the UI.
    pub location: String,
    /// How to use it.
    pub usage: String,
    /// Conditions and caveats.
    pub note: Option<String>,
}

/// Guide section: a version with its features or "Basics".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideSection {
    /// Section version ("X.Y.Z"); an empty string — the "Basics" section.
    pub version: String,
    /// Version date "DD.MM.YYYY"; empty for "Basics".
    pub date: String,
    pub entries: Vec<GuideEntry>,
}

/// The whole guide (baked into the binary). Versions — from new to old.
pub fn guide() -> Vec<GuideSection> {
    vec![
        // ── Basics ────────────────────────────────────────────────────────────
        GuideSection {
            version: String::new(),
            date: String::new(),
            entries: vec![
                GuideEntry {
                    icon: "🧭".into(),
                    title: l10n::t("Главное окно и запуск", "Main window and launch"),
                    since: None,
                    location: l10n::t(
                        "Окно приложения: пока SillyTavern не открыта — это лаунчер, после — сама SillyTavern.",
                        "App window: until SillyTavern is opened it is the launcher, afterwards it is SillyTavern itself.",
                    ),
                    usage: l10n::t(
                        "На лаунчере видно, как идёт подъём, и живёт кнопка «Открыть SillyTavern». \
Пока SillyTavern не открыта, здесь же — все вкладки настроек; после открытия настройки \
переезжают в отдельное окно (трей → «Настройки моделей»).",
                        "The launcher shows how boot is progressing and hosts the «Open SillyTavern» button. \
Until SillyTavern is opened, all settings tabs live here as well; once it is opened, the \
settings move to a separate window (tray → «Model settings»).",
                    ),
                    note: Some(l10n::t(
                        "Если подъём упал — причина показана прямо на экране, кнопка «Повторить» рядом.",
                        "If boot fails, the reason is shown right on screen, with a «Retry» button next to it.",
                    )),
                },
                GuideEntry {
                    icon: "📖".into(),
                    title: l10n::t("Это руководство", "This guide"),
                    since: None,
                    location: l10n::t(
                        "Меню системного трея → «📖 Руководство».",
                        "System tray menu → «📖 Guide».",
                    ),
                    usage: l10n::t(
                        "Все функции Cloufam: где находятся и как работают — от последнего обновления \
к самым первым. Поле сверху ищет по названию и тексту записи, разделы сворачиваются кликом.",
                        "Every Cloufam feature: where it lives and how it works — from the latest \
update back to the very first. The field at the top searches titles and entry text, sections \
collapse on click.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "✨".into(),
                    title: l10n::t("Меню жезла в SillyTavern", "The wand menu in SillyTavern"),
                    since: None,
                    location: l10n::t(
                        "Кнопка-жезл с искрами рядом с полем ввода в SillyTavern (штатное меню расширений).",
                        "The sparkling wand button next to the input field in SillyTavern (the stock extensions menu).",
                    ),
                    usage: l10n::t(
                        "Здесь собраны все инструменты Cloufam: «Модели Cloufam», «Теги персонажей», \
«Черты и характер», «Отношения», «RPG», «Импорт черновика», «Что нового», «Экспорт чата», \
«Импорт чата», «Обновить SillyTavern» и «Настройки Cloufam».",
                        "All Cloufam tools live here: «Cloufam Models», «Character Tags», «Traits & \
Character», «Relationships», «RPG», «Draft Import», «What's New», «Export Chat», «Import \
Chat», «Update SillyTavern» and «Cloufam Settings».",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "⚙️".into(),
                    title: l10n::t("Настройки", "Settings"),
                    since: None,
                    location: l10n::t(
                        "Трей → «⚙️ Настройки моделей»; горячие клавиши Ctrl+, или Alt+S; \
в SillyTavern — меню жезла → «Настройки Cloufam».",
                        "Tray → «⚙️ Model Settings»; hotkeys Ctrl+, or Alt+S; in SillyTavern — wand menu → \
«Cloufam Settings».",
                    ),
                    usage: l10n::t(
                        "Вкладки: «Чат и диалог», «Векторная память», «Зрение», «Изображения», \
«Рекомендуемые настройки», «Параметры инференса», «Другая модель», «Персонажи и теги», \
«Дообучение текста», «Дообучение картинок». Верхняя панель — статус моделей, папки и кнопка «Обновить».",
                        "Tabs: «Chat & Dialogue», «Vector Memory», «Vision», «Images», «Recommended \
Settings», «Inference Parameters», «Another Model», «Characters & Tags», «Text Training», \
«Image Training». The top bar shows model status, folders and the «Refresh» button.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "📌".into(),
                    title: l10n::t("Системный трей", "System tray"),
                    since: None,
                    location: l10n::t(
                        "Значок Cloufam в области уведомлений Windows.",
                        "The Cloufam icon in the Windows notification area.",
                    ),
                    usage: l10n::t(
                        "Левый клик — настройки. В меню: «Настройки моделей», «Открыть SillyTavern», \
«Папка с моделями», «📖 Руководство», «Выход». Пока приложение висит в трее, чат продолжает работать.",
                        "Left click — settings. The menu: «Model Settings», «Open SillyTavern», «Models \
Folder», «📖 Guide», «Exit». While the app sits in the tray, the chat keeps working.",
                    ),
                    note: Some(l10n::t(
                        "«Выход» закрывает всё приложение целиком — вместе с SillyTavern.",
                        "«Exit» closes the whole app — SillyTavern included.",
                    )),
                },
                GuideEntry {
                    icon: "🎓".into(),
                    title: l10n::t("Мастер первой настройки", "First-run setup wizard"),
                    since: None,
                    location: l10n::t(
                        "Показывается при первом запуске.",
                        "Shown on the first launch.",
                    ),
                    usage: l10n::t(
                        "Шаги: видеокарта и место на диске → скачивание рантайма (~1.7 ГБ) с прогрессом → \
модели по очереди (nomic, чат-модель, vision) → финальная индексация памяти. Vision можно пропустить \
и докачать потом из настроек.",
                        "Steps: GPU and disk space → runtime download (~1.7 GB) with progress → models one \
by one (nomic, chat model, vision) → final memory indexing. Vision can be skipped and downloaded \
later from the settings.",
                    ),
                    note: Some(l10n::t(
                        "Чекбокс «системный Ollama» на первом шаге — использовать уже установленный Ollama вместо скачивания своего.",
                        "The «system Ollama» checkbox on the first step uses an already installed Ollama instead of downloading its own.",
                    )),
                },
                GuideEntry {
                    icon: "🛒".into(),
                    title: l10n::t("Витрина моделей", "Model catalog"),
                    since: None,
                    location: l10n::t(
                        "Окно настроек: вкладки «Чат и диалог», «Векторная память», «Зрение», «Изображения». \
В SillyTavern — меню жезла → «Модели Cloufam».",
                        "Settings window: «Chat & Dialogue», «Vector Memory», «Vision», «Images» tabs. \
In SillyTavern — wand menu → «Cloufam Models».",
                    ),
                    usage: l10n::t(
                        "Показаны только модели под твоё железо: тир считается по видеопамяти и по тому, \
чем реально ускоряется инференс (CUDA или Vulkan — у AMD и Intel). Кнопка рядом с моделью скачивает; \
в меню SillyTavern то же самое с прогрессом и выбором активной модели.",
                        "Only models your hardware can run are shown: the tier is computed from VRAM and \
from what actually accelerates inference (CUDA or Vulkan — for AMD and Intel). The button next to \
a model downloads it; SillyTavern's menu does the same with progress and the active-model picker.",
                    ),
                    note: Some(l10n::t(
                        "Веса моделей лежат на складе моделей, а не в папке программы: по умолчанию Z:/AImodels, свою папку выбирают в мастере первой настройки.",
                        "Model weights live in the model store, not in the app folder: Z:/AImodels by default; a custom folder is picked in the setup wizard.",
                    )),
                },
                GuideEntry {
                    icon: "🧠".into(),
                    title: l10n::t("Векторная память", "Vector memory"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «🧠 Векторная память»; сами файлы — папка memory\\ рядом с программой.",
                        "Settings → «🧠 Vector Memory»; the files themselves — the memory\\ folder next to the app.",
                    ),
                    usage: l10n::t(
                        "Положи в memory\\ текстовые файлы (*.txt) с фактами о мире и героях — приложение \
проиндексирует их, и модель будет помнить это в разговоре. Повторная индексация не создаёт дублей.",
                        "Drop text files (*.txt) with facts about the world and the characters into memory\\ \
— the app indexes them, and the model will remember them in conversation. Re-indexing creates no \
duplicates.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "👁".into(),
                    title: l10n::t("Зрение (Vision)", "Vision"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «👁 Зрение»; используется прямо в чате.",
                        "Settings → «👁 Vision»; used right in the chat.",
                    ),
                    usage: l10n::t(
                        "Прикрепи изображение к сообщению — персонаж опишет его или ответит с учётом того, \
что видит. После анализа модель зрения выгружается, и чат работает дальше как обычно.",
                        "Attach an image to a message — the character describes it or replies taking into \
account what it sees. After the analysis the vision model unloads, and the chat goes on as usual.",
                    ),
                    note: Some(l10n::t(
                        "Модель зрения скачивается отдельно; в мастере её можно пропустить и докачать позже.",
                        "The vision model downloads separately; the wizard lets you skip it and fetch it later.",
                    )),
                },
                GuideEntry {
                    icon: "🎨".into(),
                    title: l10n::t("Генерация картинок", "Image generation"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «🎨 Изображения»; свободная генерация — команда /imagine <промпт на английском> в чате.",
                        "Settings → «🎨 Images»; free-form generation — the /imagine <prompt in English> command in chat.",
                    ),
                    usage: l10n::t(
                        "Персонаж сам рисует сцены, когда это уместно. На вкладке выбери чекпоинт и посмотри, \
как персонажи генерируют изображения; /imagine рисует что угодно по твоему промпту.",
                        "The character draws scenes on its own when it fits. On the tab, pick a checkpoint \
and watch how characters generate images; /imagine draws anything from your prompt.",
                    ),
                    note: Some(l10n::t(
                        "Только на NVIDIA (CUDA): на AMD, Intel и CPU вкладка честно скажет «генерация недоступна».",
                        "NVIDIA (CUDA) only: on AMD, Intel and CPU the tab honestly says «generation unavailable».",
                    )),
                },
                GuideEntry {
                    icon: "🎙".into(),
                    title: l10n::t("Голос (TTS)", "Voice (TTS)"),
                    since: None,
                    location: l10n::t(
                        "SillyTavern → настройки TTS (Extensions → Text-to-Speech).",
                        "SillyTavern → TTS settings (Extensions → Text-to-Speech).",
                    ),
                    usage: l10n::t(
                        "Из коробки работает System (Windows). Для качества выше включи Kokoro — он ускоряется \
видеокартой (webgpu), устройство подбирается автоматически.",
                        "System (Windows) works out of the box. For higher quality enable Kokoro — it is \
accelerated by the GPU (WebGPU), and the device is picked automatically.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🧩".into(),
                    title: l10n::t("Рекомендуемые настройки", "Recommended settings"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «🧩 Рекомендуемые настройки».",
                        "Settings → «🧩 Recommended Settings».",
                    ),
                    usage: l10n::t(
                        "Вкладка считает параметры под твою видеокарту: контекст, максимум токенов, память. \
Счётчик на вкладке показывает, сколько предложений накопилось; применяются кнопкой, дальше можно подправить руками.",
                        "The tab computes parameters for your GPU: context, max tokens, memory. The badge on \
the tab shows how many suggestions have accumulated; apply them with the button, then fine-tune by hand.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎛".into(),
                    title: l10n::t("Параметры инференса", "Inference parameters"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «⚙️ Параметры инференса».",
                        "Settings → «⚙️ Inference Parameters».",
                    ),
                    usage: l10n::t(
                        "Размер контекста, максимум токенов ответа, температура и штрафы. Значения уходят \
и в SillyTavern, и в саму модель — менять их здесь, а не в настройках ST.",
                        "Context size, reply token limit, temperature and penalties. The values go to both \
SillyTavern and the model itself — change them here, not in ST's settings.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎓".into(),
                    title: l10n::t("Дообучение", "Fine-tuning"),
                    since: None,
                    location: l10n::t(
                        "Настройки → «🎓 Дообучение текста» и «🖼 Дообучение картинок».",
                        "Settings → «🎓 Text Training» and «🖼 Image Training».",
                    ),
                    usage: l10n::t(
                        "Собери датасет (папка с примерами), выбери пресет и запусти обучение; готовый адаптер \
можно выгрузить и подключить к модели. Прогресс и отмена — на тех же экранах.",
                        "Assemble a dataset (a folder of examples), pick a preset and start training; the \
finished adapter can be exported and plugged into the model. Progress and cancel are on the same \
screens.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🔄".into(),
                    title: l10n::t("Обновление приложения", "App updates"),
                    since: None,
                    location: l10n::t(
                        "Плашка «Обновление» при старте или кнопка «🔄 Обновить» в верхней панели настроек.",
                        "The «Update» badge at startup or the «🔄 Update» button in the settings top bar.",
                    ),
                    usage: l10n::t(
                        "Нажми «Обновить» — приложение скачает новую версию, проверит подпись, подменит себя \
и вернётся уже обновлённым. Прежний exe остаётся рядом как бэкап.",
                        "Hit «Update» — the app downloads the new version, verifies the signature, replaces \
itself and comes back already updated. The previous exe stays nearby as a backup.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.78 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.78".into(),
            date: "04.11.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "📌".into(),
                    title: l10n::t("Ярлык на рабочем столе", "Desktop shortcut"),
                    since: Some("1.2.78".into()),
                    location: l10n::t(
                        "Рабочий стол. Повторно — «Настройки» → «Создать ярлык».",
                        "The desktop. Again — «Settings» → «Create shortcut».",
                    ),
                    usage: l10n::t(
                        "При запуске ярлык появляется сам, если его ещё нет. Он открывает \
установленный Cloufam и берёт иконку у программы. Удалённый руками ярлык сам не возвращается: \
кнопка в настройках создаёт его заново.",
                        "At launch the shortcut appears by itself if missing. It opens the installed \
Cloufam and takes its icon from the app. A shortcut deleted by hand does not come back on its \
own: the button in settings creates it again.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "💬".into(),
                    title: l10n::t(
                        "Реплики больше не обрываются на двоеточии",
                        "Replies no longer break on a colon",
                    ),
                    since: Some("1.2.78".into()),
                    location: l10n::t(
                        "Работает сама, настроек нет.",
                        "Works on its own, nothing to configure.",
                    ),
                    usage: l10n::t(
                        "Фраза вроде «Она вздыхает:» больше не принимается за имя персонажа, \
поэтому следующая реплика не обрезается на этом месте. Конец ответа тоже доезжает, даже если \
модель вдруг начинает говорить от другого имени.",
                        "A phrase like «She sighs:» is no longer mistaken for a character name, so the \
next reply is not cut there. The end of a reply also arrives in full, even if the model suddenly \
starts speaking under a different name.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎲".into(),
                    title: l10n::t("Награды, навыки и трактирщик", "Rewards, skills and the innkeeper"),
                    since: Some("1.2.78".into()),
                    location: l10n::t(
                        "Ролевая сцена: квесты, бой, навыки, таверна.",
                        "Roleplay scenes: quests, combat, skills, the tavern.",
                    ),
                    usage: l10n::t(
                        "Награда за выполненный квест выдаётся, даже если следующее сообщение \
пришло, пока считался опыт. Опыт за бой не теряет остаток. Навык, привязанный к «Ловкости» или \
другому русскому названию характеристики, влияет на бой. Трактирщик остаётся в таверне, когда \
рядом появляются новые персонажи.",
                        "A reward for a completed quest is granted even if the next message arrived \
while the experience was being counted. Combat experience no longer loses its remainder. A skill \
bound to «Ловкость» or another Russian stat name affects combat. The innkeeper stays in the tavern \
when new characters appear nearby.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.77 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.77".into(),
            date: "02.11.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "💬".into(),
                    title: l10n::t("Защита от эхо в конце ответов", "Echo guard at the end of replies"),
                    since: Some("1.2.77".into()),
                    location: l10n::t(
                        "Работает сама, настроек нет.",
                        "Works on its own, nothing to configure.",
                    ),
                    usage: l10n::t(
                        "Если ответ начинает дословно повторять прошлые сообщения (твои или \
персонажа), повтор обрезается на месте — свежая часть реплики доезжает целиком. Продолжение \
реплики кнопкой «Continue» не режется никогда. Заодно расширено окно штрафа за повторы: \
текст в сообщениях и мыслях ~таких~ повторяется заметно реже.",
                        "If a reply starts repeating past messages verbatim (yours or the character's), \
the repeat is cut on the spot — the fresh part of the reply arrives in full. A reply continued \
with the «Continue» button is never cut. The repeat-penalty window was widened at the same time: \
text in messages and in ~thoughts~ repeats noticeably less often.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.63 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.63".into(),
            date: "12.10.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "⚡".into(),
                    title: l10n::t("Модели Qwen 3.5", "Qwen 3.5 models"),
                    since: Some("1.2.63".into()),
                    location: l10n::t(
                        "Окно настроек → вкладки «Чат и диалог», «Зрение», «Векторная память»; \
в SillyTavern — меню жезла → «Модели Cloufam».",
                        "Settings window → «Chat & Dialogue», «Vision», «Vector Memory» tabs; \
in SillyTavern — wand menu → «Cloufam Models».",
                    ),
                    usage: l10n::t(
                        "Новый ряд без цензуры: чат и зрение работают на одних и тех же \
мультимодальных моделях — от 0.8B для слабых машин до флагманской 9b. Витрина показывает только \
то, что потянет твоя машина; кнопка рядом с моделью скачивает её, «★ Активна» — текущая. Модели \
нового поколения заметно быстрее прежних и живее держат характер.",
                        "The new uncensored line: chat and vision run on the very same multimodal \
models — from 0.8B for weak machines to the flagship 9b. The catalog shows only what your machine \
can handle; the button next to a model downloads it, «★ Active» marks the current one. The new \
generation is noticeably faster and holds character more vividly.",
                    ),
                    note: Some(l10n::t(
                        "9b требует 11 ГБ+ видеопамяти: на 8 ГБ картах каталог предлагает 4B.",
                        "9b needs 11 GB+ of VRAM: on 8 GB cards the catalog offers the 4B.",
                    )),
                },
                GuideEntry {
                    icon: "🧰".into(),
                    title: l10n::t(
                        "Обновление модели с удалением старой",
                        "Model upgrade with removal of the old one",
                    ),
                    since: Some("1.2.63".into()),
                    location: l10n::t(
                        "Окно настроек → вкладки «Чат и диалог» и «Зрение» — карточка \
«Доступна модель нового поколения» над витриной.",
                        "Settings window → «Chat & Dialogue» and «Vision» tabs — the «A new-generation \
model is available» card above the catalog.",
                    ),
                    usage: l10n::t(
                        "Если на диске стоит модель прежнего ряда, появляется предложение: \
«Обновить и удалить старую» скачает новую, переведёт на неё все роли (чат, зрение, оценщика) \
и удалит старую, освободив место. «Только удалить старую» — если новая не нужна; «Позже» — \
спрятать предложение до следующих обновлений.",
                        "If a model from the former line is installed, an offer appears: «Upgrade and \
remove the old one» downloads the new model, moves every role onto it (chat, vision, evaluator) \
and deletes the old one, freeing space. «Only remove the old one» — if the new one is not \
wanted; «Later» — hide the offer until future updates.",
                    ),
                    note: Some(l10n::t(
                        "Старая удаляется только после успешной загрузки новой — без модели не останешься.",
                        "The old one is deleted only after the new one downloads successfully — you never end up empty-handed.",
                    )),
                },
                GuideEntry {
                    icon: "🔄".into(),
                    title: l10n::t("Ollama обновляется сама", "Ollama updates itself"),
                    since: Some("1.2.63".into()),
                    location: l10n::t(
                        "Работает автоматически при запуске; прогресс — на экране загрузки.",
                        "Runs automatically at launch; progress is on the loading screen.",
                    ),
                    usage: l10n::t(
                        "Новая версия встроенной Ollama скачивается с докачкой и ставится поверх \
старой, модели не трогаются. Если сеть подвела — приложение запускается со старой версией \
и попробует снова при следующем старте.",
                        "The new version of the bundled Ollama downloads with resume support and installs \
over the old one; models are untouched. If the network fails, the app launches with the old \
version and tries again at the next start.",
                    ),
                    note: Some(l10n::t(
                        "Системный Ollama (настройка «использовать системный») не обновляется — это чужой рантайм.",
                        "A system Ollama (the «use system Ollama» setting) is never updated — it is someone else's runtime.",
                    )),
                },
            ],
        },
        // ── 1.2.60 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.60".into(),
            date: "08.10.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🤝".into(),
                    title: l10n::t(
                        "Связи персонажей: кто о ком думает",
                        "Character bonds: who thinks what about whom",
                    ),
                    since: Some("1.2.60".into()),
                    location: l10n::t(
                        "Окно Cloufam → «🏷 Персонажи и теги» → карточка персонажа → «Связи с другими \
персонажами». Просмотр: меню жезла → «Отношения» → раздел «Что он о них думает».",
                        "Cloufam window → «🏷 Characters & Tags» → character card → «Bonds with other \
characters». Viewing: wand menu → «Relationships» → the «What they think of them» section.",
                    ),
                    usage: l10n::t(
                        "Добавь связь: к кому, кем приходится, насколько сильно (0–100), о чём можно \
сплетничать и галочка «он не знает». Мнение уходит в карточку и работает в групповом чате: \
каждый держит своё мнение об остальных, а та, о ком пишут за спиной, не догадывается.",
                        "Add a bond: to whom, in what capacity, how strong (0–100), what gossip fits, and \
an «they don't know» checkbox. The opinion goes into the card and works in group chats: everyone \
keeps their own opinion of the others, and the one being talked about behind their back has no idea.",
                    ),
                    note: Some(l10n::t(
                        "Правка — только в окне Cloufam; автозаполнения по переписке нет, слой ручной.",
                        "Editing is only in the Cloufam window; there is no auto-fill from chat history — the layer is manual.",
                    )),
                },
            ],
        },
        // ── 1.2.59 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.59".into(),
            date: "06.10.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "💾".into(),
                    title: l10n::t("Экспорт чата", "Chat export"),
                    since: Some("1.2.59".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «Экспорт чата».",
                        "SillyTavern → wand menu → «Export Chat».",
                    ),
                    usage: l10n::t(
                        "Сохраняет копию открытого чата в файл (jsonl) в папку chat-backups \
и показывает путь к нему тостом.",
                        "Saves a copy of the open chat to a file (jsonl) in the chat-backups folder \
and shows its path in a toast.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "📥".into(),
                    title: l10n::t("Импорт чата", "Chat import"),
                    since: Some("1.2.59".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «Импорт чата».",
                        "SillyTavern → wand menu → «Import Chat».",
                    ),
                    usage: l10n::t(
                        "Поднимает привычную загрузку чата SillyTavern: выбери файл json или jsonl — \
чат появится в списке.",
                        "Brings up SillyTavern's familiar chat import: pick a json or jsonl file — \
the chat appears in the list.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.58 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.58".into(),
            date: "02.10.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "✏️".into(),
                    title: l10n::t("Правка любого сообщения", "Edit any message"),
                    since: Some("1.2.58".into()),
                    location: l10n::t(
                        "Карандаш на любом сообщении в чате — своём, персонажа, служебном и приветственном.",
                        "A pencil on any chat message — yours, the character's, system and greeting ones.",
                    ),
                    usage: l10n::t(
                        "Нажми карандаш, поправь текст и сохрани. Правка сообщения персонажа запускает \
новую генерацию от этого места.",
                        "Click the pencil, fix the text and save. Editing a character message starts a \
new generation from that point.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "♻️".into(),
                    title: l10n::t(
                        "Автоперегенерация дословных повторов",
                        "Auto-regeneration of verbatim repeats",
                    ),
                    since: Some("1.2.58".into()),
                    location: l10n::t(
                        "Работает автоматически, настраивать нечего.",
                        "Works automatically, nothing to configure.",
                    ),
                    usage: l10n::t(
                        "Если модель вместо нового ответа дословно пересказывает сообщение из истории, \
генерация тихо перезапускается без скопированного сообщения — руками ничего делать не нужно.",
                        "If the model retells a message from history verbatim instead of a new reply, \
generation quietly restarts without the copied message — there is nothing to do by hand.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.57 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.57".into(),
            date: "01.10.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🗣".into(),
                    title: l10n::t("Ответ на твоём языке", "Replies in your language"),
                    since: Some("1.2.57".into()),
                    location: l10n::t(
                        "Работает автоматически, настроек нет.",
                        "Works automatically, nothing to configure.",
                    ),
                    usage: l10n::t(
                        "Пишешь по-русски — ответ целиком по-русски; переписался на английском — \
персонаж подхватывает английский. Языки не смешиваются даже с англоязычной карточкой.",
                        "Write in Russian — the reply is fully Russian; switch to English — the character \
picks up English. Languages never mix, even with an English card.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.54 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.54".into(),
            date: "26.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🗺️".into(),
                    title: l10n::t("Лорбуки (World Info)", "Lorebooks (World Info)"),
                    since: Some("1.2.54".into()),
                    location: l10n::t(
                        "SillyTavern → World Info (лорбуки); ключевые слова — внутри записи книги.",
                        "SillyTavern → World Info (lorebooks); keywords live inside the book's entries.",
                    ),
                    usage: l10n::t(
                        "Импортируй книгу — она сразу включается, и её ключи действительно активируют \
записи по словам из реплики. Английские книги с 1.2.56 получают русские ключи автоматически — \
работают оба языка. Книгу можно заполнять с первой же открытой записи.",
                        "Import a book — it enables right away, and its keys actually trigger entries by \
words from the reply. Since 1.2.56 English books get Russian keywords automatically — both \
languages work. A book can be filled starting from its very first open entry.",
                    ),
                    note: Some(l10n::t(
                        "Служебные поля content/comment не переводятся никогда; перевод текста — только по тумблеру перевода.",
                        "The service fields content/comment are never translated; text translation happens only via the translation toggle.",
                    )),
                },
                GuideEntry {
                    icon: "🌐".into(),
                    title: l10n::t(
                        "Перевод импортированных карточек и лорбуков",
                        "Translating imported cards and lorebooks",
                    ),
                    since: Some("1.2.54".into()),
                    location: l10n::t(
                        "Окно Cloufam → «🏷 Персонажи и теги» → «Переводить новые карточки и лорбуки \
на русский автоматически».",
                        "Cloufam window → «🏷 Characters & Tags» → «Automatically translate new cards \
and lorebooks into Russian».",
                    ),
                    usage: l10n::t(
                        "Выключен по умолчанию (с 1.2.57): импорт оставляет карточку ровно такой, какой \
её сделал автор. Включи тумблер — новые импорты переводятся локальной моделью: описание, характер, \
сценарий и реплики. Исходник сохраняется внутри карточки, имена и макросы не трогаются.",
                        "Off by default (since 1.2.57): import leaves the card exactly as the author made \
it. Flip the toggle — new imports are translated by the local model: description, personality, \
scenario and lines. The original is kept inside the card; names and macros are untouched.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.53 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.53".into(),
            date: "25.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🤝".into(),
                    title: l10n::t("Живое состояние персонажа", "Character live state"),
                    since: Some("1.2.53".into()),
                    location: l10n::t(
                        "Окно Cloufam → «🏷 Персонажи и теги» → «Черты характера» → «Живое состояние»; \
то же — меню жезла → «Черты и характер».",
                        "Cloufam window → «🏷 Characters & Tags» → «Character Traits» → «Live State»; \
same in the wand menu → «Traits & Character».",
                    ),
                    usage: l10n::t(
                        "Состояние меняется само по ходу разговора — его ведёт фоновый оценщик — и уходит \
в промпт отдельной заметкой, поэтому персонаж помнит, что с ним только что произошло. Значение \
можно поправить руками, в том числе пока чат открыт.",
                        "The state changes on its own as the conversation goes — a background evaluator \
tracks it — and reaches the prompt as a separate note, so the character remembers what just \
happened to them. You can adjust the value by hand, even while the chat is open.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "⚡".into(),
                    title: l10n::t("Быстрые ответы", "Faster replies"),
                    since: Some("1.2.53".into()),
                    location: l10n::t(
                        "Работает автоматически, настраивать нечего.",
                        "Works automatically, nothing to configure.",
                    ),
                    usage: l10n::t(
                        "Модель держит уже прочитанный разговор в памяти и не перезагружается на каждый \
служебный вызов (оценку, ремарку, теги) — ответы приходят в разы быстрее, а длинные реплики \
печатаются плавнее.",
                        "The model keeps the already-read conversation in memory and does not reload on \
every service call (evaluation, remark, tags) — replies arrive several times faster, and long \
lines type out more smoothly.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.52 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.52".into(),
            date: "21.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🎬".into(),
                    title: l10n::t(
                        "Ремарки не путаются с репликами",
                        "Stage directions no longer mix with lines",
                    ),
                    since: Some("1.2.52".into()),
                    location: l10n::t(
                        "Ремарка — Ctrl+Enter (или Alt+Enter) с текстом; тот же текст в режиме режиссёра.",
                        "A stage direction — Ctrl+Enter (or Alt+Enter) with text; the same text in director mode.",
                    ),
                    usage: l10n::t(
                        "Опиши, что происходит в сцене: ремарка уходит модели как указание рассказчика, \
а не как твоя реплика — персонаж реагирует на событие, а не отвечает за тебя. С 1.2.56 описанное \
в ремарке происходит прямо сейчас, и директива остаётся последним словом в промпте.",
                        "Describe what happens in the scene: the direction reaches the model as a narrator's \
cue, not as your line — the character reacts to the event instead of answering for you. Since \
1.2.56 whatever the direction describes happens right now, and the directive stays the last word \
of the prompt.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.49 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.49".into(),
            date: "20.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🔄".into(),
                    title: l10n::t("Обновление встроенной SillyTavern", "Updating the bundled SillyTavern"),
                    since: Some("1.2.49".into()),
                    location: l10n::t(
                        "Автоматически при запуске; вручную — меню жезла → «Обновить SillyTavern» (с 1.2.59).",
                        "Automatically at launch; manually — wand menu → «Update SillyTavern» (since 1.2.59).",
                    ),
                    usage: l10n::t(
                        "При выходе новой версии SillyTavern приложение само её скачает и поставит — \
на уже работающей установке, без потери персонажей и чатов. Пункт меню показывает установленную \
и последнюю версию и обновляет сразу, перезапуская SillyTavern на том же адресе.",
                        "When a new SillyTavern version comes out, the app downloads and installs it by \
itself — on the existing installation, without losing characters and chats. The menu item shows \
the installed and the latest version and updates right away, restarting SillyTavern at the same \
address.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.48 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.48".into(),
            date: "19.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "📋".into(),
                    title: l10n::t("Импорт черновика в карточку", "Draft import into a card"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «Импорт черновика».",
                        "SillyTavern → wand menu → «Draft Import».",
                    ),
                    usage: l10n::t(
                        "Вставь в окно заметки, обрывки диалога или прозу про персонажа — локальная модель \
разложит текст по полям карточки (имя, описание, характер, сценарий, приветствие, пример диалога). \
Проверь результат и заполни форму создания персонажа одной кнопкой.",
                        "Paste notes, dialogue fragments or prose about the character into the window — the \
local model splits the text across the card fields (name, description, personality, scenario, \
greeting, example dialogue). Check the result and fill the character creation form with one button.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "💭".into(),
                    title: l10n::t("Мысли персонажа", "Character thoughts"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "SillyTavern → настройки регулярных выражений (Regex Presets).",
                        "SillyTavern → regex settings (Regex Presets).",
                    ),
                    usage: l10n::t(
                        "Оборачивай внутреннюю речь в тильды: ~вот так~ — в чате она покажется \
лавандовым курсивом, отдельно от *действий в звёздочках* и «реплик». С 1.2.54 подсветка включена \
по умолчанию, а модели подсказывается писать мысли в тильдах. Формат не нужен — выключи скрипт в Regex Presets.",
                        "Wrap internal speech in tildes: ~like this~ — in chat it shows as lavender \
italics, separate from *starred actions* and «lines». Since 1.2.54 the highlight is on by default, \
and the model is nudged to write thoughts in tildes. If you do not want the format — disable the \
script in Regex Presets.",
                    ),
                    note: Some(l10n::t(
                        "Правка только для отображения: в промпте тильды остаются, модель видит свой формат.",
                        "Display-only tweak: the tildes stay in the prompt, the model sees its own format.",
                    )),
                },
                GuideEntry {
                    icon: "🎲".into(),
                    title: l10n::t("Бросок костей /roll", "Dice roll /roll"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "Поле ввода чата: /roll 2d6+3 (или любая комбинация NdM+K).",
                        "Chat input: /roll 2d6+3 (or any NdM+K combination).",
                    ),
                    usage: l10n::t(
                        "Кости кидает браузер, результат приходит ремаркой от твоего лица. Можно бросать \
в любую сторону сцены — это нарративная мелочь, на механику RPG не влияет.",
                        "The browser rolls the dice, and the result arrives as a stage direction from you. \
You can roll for anyone in the scene — it is a narrative trifle with no effect on RPG mechanics.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🔍".into(),
                    title: l10n::t("Трасса промпта", "Prompt trace"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "Папка data\\traces\\ рядом с программой; список трасс — адрес \
http://127.0.0.1:<порт шлюза>/api/cloufam/traces в браузере.",
                        "The data\\traces\\ folder next to the app; the trace list — the address \
http://127.0.0.1:<gateway port>/api/cloufam/traces in a browser.",
                    ),
                    usage: l10n::t(
                        "Каждый запрос к модели пишется в трассу: точные сообщения, что реально ушло \
в модель, её ответ и метрики Ollama. Разбирается, когда ответ выглядит странно — видно, чем \
модель на самом деле кормили.",
                        "Every request to the model is written to a trace: the exact messages that actually \
went to the model, its reply and Ollama metrics. Use it when a reply looks odd — you can see \
what the model was actually fed.",
                    ),
                    note: Some(l10n::t(
                        "Трассы хранятся несколько дней и удаляются автоматически.",
                        "Traces are kept for a few days and deleted automatically.",
                    )),
                },
                GuideEntry {
                    icon: "🧠".into(),
                    title: l10n::t("Модель фонового оценщика", "Background evaluator model"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "Окно Cloufam → «🏷 Персонажи и теги» → «Оценщик состояния, RPG и режиссёр» → \
«Модель для оценщика».",
                        "Cloufam window → «🏷 Characters & Tags» → «State evaluator, RPG and director» → \
«Evaluator model».",
                    ),
                    usage: l10n::t(
                        "Пусто — модель выбирается автоматически по видеопамяти. Напиши свою — оценщик \
(живое состояние, характеристики, навыки, отношения, ремарки) будет работать на ней, если автовыбор \
ведёт себя нестабильно.",
                        "Empty — the model is chosen automatically by VRAM. Enter your own — the evaluator \
(live state, stats, skills, relationships, stage directions) will run on it, if the automatic \
choice behaves unstably.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎬".into(),
                    title: l10n::t("Свои указания режиссёру и RPG", "Custom instructions for the director and RPG"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "Окно Cloufam → «🏷 Персонажи и теги» → «Оценщик состояния, RPG и режиссёр»: \
три поля — «Указания режиссёру», «Указания оценщику состояния и RPG», «Указания оценщику боя».",
                        "Cloufam window → «🏷 Characters & Tags» → «State evaluator, RPG and director»: \
three fields — «Instructions for the director», «Instructions for the state and RPG evaluator», \
«Instructions for the combat evaluator».",
                    ),
                    usage: l10n::t(
                        "Допиши свои указания к встроенным промптам: стиль разворота ремарки в сцену, \
оценку реплики, раунд боя. Сохраняются кнопкой внизу блока.",
                        "Add your own instructions to the built-in prompts: the style of turning a remark \
into a scene, how a reply is evaluated, a combat round. Saved with the button at the bottom of \
the block.",
                    ),
                    note: Some(l10n::t(
                        "Добавляются к встроенному промпту — заменить его целиком нельзя.",
                        "They are appended to the built-in prompt — replacing it entirely is not possible.",
                    )),
                },
                GuideEntry {
                    icon: "🎛".into(),
                    title: l10n::t("Именованные пресеты сэмплеров", "Named sampler presets"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "SillyTavern → TextGen Settings (настройки подключения), список пресетов.",
                        "SillyTavern → TextGen Settings (connection settings), the preset list.",
                    ),
                    usage: l10n::t(
                        "Три готовых пресета: «сбалансированный», «меньше повторов», «предсказуемый». \
Переключаются штатной кнопкой SillyTavern.",
                        "Three ready-made presets: «balanced», «fewer repeats», «predictable». \
Switched with SillyTavern's stock button.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎙".into(),
                    title: l10n::t("Kokoro TTS с ускорением", "Kokoro TTS with acceleration"),
                    since: Some("1.2.48".into()),
                    location: l10n::t(
                        "SillyTavern → настройки TTS → Kokoro.",
                        "SillyTavern → TTS settings → Kokoro.",
                    ),
                    usage: l10n::t(
                        "Голос af_heart — лучший по качеству; устройство поднимается с wasm (CPU) на \
webgpu — на порядок быстрее. Свой выбор устройства не перетирается.",
                        "The af_heart voice is the best in quality; the device is promoted from wasm (CPU) \
to WebGPU — an order of magnitude faster. Your own device choice is never overwritten.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.47 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.47".into(),
            date: "15.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "⬇️".into(),
                    title: l10n::t("Управление загрузками моделей", "Managing model downloads"),
                    since: Some("1.2.47".into()),
                    location: l10n::t(
                        "Окно настроек: вкладки моделей и плашка загрузок; в SillyTavern — \
меню жезла → «Модели Cloufam».",
                        "Settings window: the model tabs and the downloads badge; in SillyTavern — \
wand menu → «Cloufam Models».",
                    ),
                    usage: l10n::t(
                        "У ожидающей модели кнопка «Качать эту» — она пойдёт следующей, а прежняя \
вернётся в очередь, сохранив всё скачанное. Пауза/«Продолжить» и «Отмена» действуют только на \
свою строку и не трогают остальные загрузки.",
                        "A queued model has a «Download this» button — it goes next, while the previous \
one returns to the queue keeping everything downloaded. Pause/«Resume» and «Cancel» affect only \
their own row and leave the other downloads alone.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.46 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.46".into(),
            date: "14.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🏷".into(),
                    title: l10n::t("Теги персонажей", "Character tags"),
                    since: Some("1.2.46".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «🏷 Теги персонажей»; авто-теги — окно Cloufam → \
«🏷 Персонажи и теги» → «Авто-теги персонажей SillyTavern».",
                        "SillyTavern → wand menu → «🏷 Character Tags»; auto-tags — Cloufam window → \
«🏷 Characters & Tags» → «SillyTavern character auto-tags».",
                    ),
                    usage: l10n::t(
                        "Русские метки вместо мешанины английских кличек: поколение, образ, роль и \
3–5 черт характера. По ним работают фильтр библиотеки и подбор похожих персонажей. Авто-теги — \
по включению (с 1.2.57 выключены по умолчанию); карточки со своими тегами не переписываются никогда.",
                        "Russian labels instead of a mess of English nicknames: generation, image, role \
and 3–5 character traits. The library filter and the similar-characters picker work off them. \
Auto-tags are opt-in (off by default since 1.2.57); cards that already have their own tags are \
never overwritten.",
                    ),
                    note: Some(l10n::t(
                        "Снятая в окне Cloufam галочка применяется к живой странице сразу.",
                        "A checkbox cleared in the Cloufam window applies to the live page immediately.",
                    )),
                },
            ],
        },
        // ── 1.2.43 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.43".into(),
            date: "13.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "📦".into(),
                    title: l10n::t("Откат версий", "Version rollback"),
                    since: Some("1.2.43".into()),
                    location: l10n::t(
                        "Плашка «Обновление» при старте → «Все версии ▾».",
                        "The «Update» badge at startup → «All versions ▾».",
                    ),
                    usage: l10n::t(
                        "Список всех релизов на GitHub с датами и описаниями. Выбери версию — она \
скачается и установится, а текущий exe сохранится как бэкап на случай, если захочешь вернуться.",
                        "A list of all GitHub releases with dates and descriptions. Pick a version — it \
downloads and installs, and the current exe is kept as a backup in case you want to go back.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.39 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.39".into(),
            date: "13.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "🎬".into(),
                    title: l10n::t("Режим режиссёра", "Director mode"),
                    since: Some("1.2.39".into()),
                    location: l10n::t(
                        "Кнопка 🎬 рядом с полем ввода; Ctrl+Enter (Alt+Enter) с текстом; префиксы // и /d в начале сообщения.",
                        "The 🎬 button next to the input field; Ctrl+Enter (Alt+Enter) with text; // and /d prefixes at the start of a message.",
                    ),
                    usage: l10n::t(
                        "Клик по кнопке — включить/выключить режим (в нём каждое сообщение уходит как \
ремарка сцены), Shift+клик — настройки режиссёра. В обычном режиме // или /d в начале строки \
отправит ремарку разово. Ремарка — это событие сцены, а не твоя реплика.",
                        "Click the button to toggle the mode (every message goes as a scene direction), \
Shift+click — director settings. In the normal mode, // or /d at the start of a line sends a \
direction once. A direction is a scene event, not your line.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🏪".into(),
                    title: l10n::t("Магазины через режиссёра", "Shops via the director"),
                    since: Some("1.2.39".into()),
                    location: l10n::t(
                        "Ремарка режиссёра: // shop зелья | оружие | подарки; убрать — // shop_remove.",
                        "Director remark: // shop potions | weapons | gifts; remove — // shop_remove.",
                    ),
                    usage: l10n::t(
                        "В текущей локации появится магазин нужного типа; тип влияет на ассортимент. \
В городе товары дешевле, в лесу — дороже.",
                        "A shop of the requested type appears in the current location; the type affects \
the stock. Goods are cheaper in town, pricier in the forest.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎉".into(),
                    title: l10n::t("«Что нового»", "«What's New»"),
                    since: Some("1.2.39".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «Что нового»; само показывается после обновления.",
                        "SillyTavern → wand menu → «What's New»; also shown by itself after an update.",
                    ),
                    usage: l10n::t(
                        "Журнал обновлений: после каждого обновления — только то, что ты пропустил \
(с 1.2.52), а из меню — вся история версий.",
                        "The update log: after each update — only what you missed (since 1.2.52), and \
from the menu — the full version history.",
                    ),
                    note: None,
                },
            ],
        },
        // ── 1.2.36 ───────────────────────────────────────────────────────────
        GuideSection {
            version: "1.2.36".into(),
            date: "13.09.2026".into(),
            entries: vec![
                GuideEntry {
                    icon: "⚔".into(),
                    title: l10n::t("RPG-панель", "RPG panel"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «RPG».",
                        "SillyTavern → wand menu → «RPG».",
                    ),
                    usage: l10n::t(
                        "Всё RPG в одном окне, шесть вкладок: 🌍 Мир, 📊 Характеристики, 🎯 Навыки, \
📜 Квесты, 🎒 Инвентарь, ⚔ Бой. Мир и бой начинаются именно отсюда.",
                        "All RPG in one window, six tabs: 🌍 World, 📊 Stats, 🎯 Skills, 📜 Quests, 🎒 \
Inventory, ⚔ Combat. The world and combat start right here.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🗺️".into(),
                    title: l10n::t("Мир и локации", "World and locations"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «🌍 Мир».",
                        "RPG panel → «🌍 World».",
                    ),
                    usage: l10n::t(
                        "У каждой локации — название, описание и атмосфера (тревожная, весёлая, мрачная); \
локации связаны путями, можно ходить. Переход в неизвестное место генерирует новую локацию \
автоматически или вручную через режиссёра. Атмосфера влияет на события.",
                        "Every location has a name, a description and an atmosphere (tense, cheerful, \
gloomy); locations are connected by paths you can walk. Moving into an unknown place generates a \
new location automatically or by hand via the director. Atmosphere affects events.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "📊".into(),
                    title: l10n::t("Характеристики", "Stats"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «📊 Характеристики».",
                        "RPG panel → «📊 Stats».",
                    ),
                    usage: l10n::t(
                        "Шесть базовых: Сила, Выносливость, Ловкость, Интеллект, Харизма, Восприятие. \
Растут от использования в диалоге; с 1.2.39 добавлен общий опыт за квесты и бои — оценщик сам \
распределяет его по релевантным характеристикам. Уровень 1–10, прогресс виден в панели.",
                        "Six basics: Strength, Stamina, Dexterity, Intellect, Charisma, Perception. They \
grow from use in dialogue; since 1.2.39 there is shared experience from quests and combat — the \
evaluator distributes it across the relevant stats itself. Levels 1–10, progress is visible in \
the panel.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎯".into(),
                    title: l10n::t("Навыки", "Skills"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «🎯 Навыки».",
                        "RPG panel → «🎯 Skills».",
                    ),
                    usage: l10n::t(
                        "Ближний бой, Скрытность, Убеждение, Выживание, Ремесло, Медицина — растут \
от использования медленнее характеристик. Свои навыки добавляются через режиссёра и привязываются \
к любой характеристике (например, «Массаж» → Ловкость).",
                        "Melee, Stealth, Persuasion, Survival, Crafting, Medicine — grow from use, slower \
than stats. Custom skills are added via the director and bound to any stat (for example, \
«Massage» → Dexterity).",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "📜".into(),
                    title: l10n::t("Квесты", "Quests"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «📜 Квесты».",
                        "RPG panel → «📜 Quests».",
                    ),
                    usage: l10n::t(
                        "Квест — цепочка из 2–5 шагов с конкретными целями. Квестодатель — NPC или \
режиссёр; за выполнение дают опыт, предметы и сдвиг отношений. Стадии продвигаются сами по ходу \
диалога — нажимать кнопки не нужно. Новый квест под текущую локацию создаёт кнопка \
«✨ Сгенерировать квест» на вкладке «📜 Квесты».",
                        "A quest is a chain of 2–5 steps with concrete objectives. The giver is an NPC \
or the director; completion grants experience, items and a relationship shift. Stages advance by \
themselves as the dialogue goes — no buttons to press. The «✨ Generate quest» button on the \
«📜 Quests» tab creates a new quest for the current location.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "👤".into(),
                    title: l10n::t("NPC", "NPCs"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «🌍 Мир»; своё мнение о них — «Что он о них думает» (связи, 1.2.60).",
                        "RPG panel → «🌍 World»; opinions about them — «What they think of them» (bonds, 1.2.60).",
                    ),
                    usage: l10n::t(
                        "У каждого NPC — имя, внешность, характер и свои отношения с игроком. \
Они реагируют на твои действия, появляются и исчезают по сюжету; в локации показываются по именам. \
Магазины и квесты привязаны к NPC. Кнопка «✨ Описать NPC в локации» на вкладке «🌍 Мир» добавляет \
нового жителя с внешностью и характером.",
                        "Every NPC has a name, an appearance, a personality and their own relationship \
with the player. They react to your actions, appear and disappear with the story; locations list \
them by name. Shops and quests are bound to NPCs. The «✨ Describe an NPC in the location» button \
on the «🌍 World» tab adds a new resident with an appearance and a personality.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "🎒".into(),
                    title: l10n::t("Инвентарь и торговля", "Inventory and trade"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «🎒 Инвентарь»; прилавок торговца — там же, покупка — кнопкой «Купить».",
                        "RPG panel → «🎒 Inventory»; the merchant's stall is there too, buying — with the «Buy» button.",
                    ),
                    usage: l10n::t(
                        "Предметы с редкостью от обычного до легендарного. Цена зависит от места \
(в городе дешевле), отношений (другу скидка) и редкости — торговаться можно. В локациях с магазином \
на вкладке инвентаря виден прилавок NPC с ценами: «Купить» списывает ценности из инвентаря. Подарки \
(серьги, кольца) дари компаньону — отношения сдвинутся.",
                        "Items with rarity from common to legendary. The price depends on the place \
(cheaper in town), relationships (a discount for a friend) and rarity — haggling is possible. In \
locations with a shop, the NPC's stall with prices is visible on the inventory tab: «Buy» spends \
valuables from the inventory. Give gifts (earrings, rings) to a companion — the relationship will \
shift.",
                    ),
                    note: None,
                },
                GuideEntry {
                    icon: "⚔".into(),
                    title: l10n::t("Бой", "Combat"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "RPG-панель → «⚔ Бой»; действия пишутся текстом в чат.",
                        "RPG panel → «⚔ Combat»; actions are typed as text in the chat.",
                    ),
                    usage: l10n::t(
                        "С 1.2.39 бой — свободный диалог: просто опиши, что делаешь («атакую», \
«запугиваю», «предлагаю разговор») — оценщик определит режим и нужный навык. У врага свои \
характеристики (Выносливость, Сила, Мораль): сломал одну — он сдаётся, добивать не нужно. \
Законченный бой уходит из промпта, состояние сохраняется между запусками.",
                        "Since 1.2.39 combat is free-form dialogue: just describe what you do («attack», \
«intimidate», «offer to talk») — the evaluator determines the mode and the right skill. The enemy \
has its own stats (Stamina, Strength, Morale): break one and it surrenders, no need for a killing \
blow. A finished fight leaves the prompt, and its state persists between launches.",
                    ),
                    note: Some(l10n::t(
                        "Враги без флага «negotiable» (волки, звери) не ведут переговоры и не берут подкуп (с 1.2.44); подкуп работает только в режиме переговоров. Исход переговоров решает навык Persuasion персонажа — модель оценивает лишь убедительность аргумента (−1/0/+1 к навыку).",
                        "Enemies without the «negotiable» flag (wolves, beasts) do not negotiate and take no bribes (since 1.2.44); bribery works only in negotiation mode. Negotiation outcomes are decided by the character's Persuasion skill — the model only grades how convincing the argument is (−1/0/+1 to the skill).",
                    )),
                },
                GuideEntry {
                    icon: "🤝".into(),
                    title: l10n::t("Отношения", "Relationships"),
                    since: Some("1.2.36".into()),
                    location: l10n::t(
                        "SillyTavern → меню жезла → «Отношения».",
                        "SillyTavern → wand menu → «Relationships».",
                    ),
                    usage: l10n::t(
                        "Пять независимых шкал: Дружба, Романтика, Уважение, Страх, Ненависть — \
меняются сами от того, как ты общаешься. Модалка показывает текущие значения и автоматическую \
оценку по переписке; с 1.2.60 она живёт отдельным разделом, а «Что он о них думает» показывает \
связи персонажа.",
                        "Five independent scales: Friendship, Romance, Respect, Fear, Hatred — they \
change on their own based on how you treat each other. The modal shows current values and an \
automatic assessment from the correspondence; since 1.2.60 it lives in a separate section, while \
«What they think of them» shows the character's bonds.",
                    ),
                    note: None,
                },
            ],
        },
    ]
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_not_empty_and_basics_first() {
        let g = guide();
        assert!(!g.is_empty(), "руководство не должно быть пустым");
        assert_eq!(g[0].version, "", "первым идёт раздел «Основы»");
        assert!(
            g.iter().skip(1).all(|s| !s.version.is_empty()),
            "раздел «Основы» ровно один и стоит первым"
        );
    }

    #[test]
    fn versions_sorted_descending() {
        let g = guide();
        for pair in g.windows(2) {
            // The empty "Basics" version in the first row is already checked; it does not parse.
            if pair[0].version.is_empty() || pair[1].version.is_empty() {
                continue;
            }
            let a: Vec<u32> = pair[0]
                .version
                .split('.')
                .map(|s| s.parse().unwrap())
                .collect();
            let b: Vec<u32> = pair[1]
                .version
                .split('.')
                .map(|s| s.parse().unwrap())
                .collect();
            assert!(
                a > b,
                "версии идут от новой к старой без дублей: {} > {}",
                pair[0].version,
                pair[1].version
            );
        }
    }

    #[test]
    fn entries_have_required_fields() {
        for section in guide() {
            assert!(!section.entries.is_empty(), "пустой раздел {}", section.version);
            for entry in &section.entries {
                assert!(!entry.icon.is_empty(), "{}: нет эмодзи", entry.title);
                assert!(!entry.title.is_empty(), "запись без названия");
                assert!(!entry.location.is_empty(), "{}: не указано место", entry.title);
                assert!(!entry.usage.is_empty(), "{}: не описано использование", entry.title);
                // Basics have no version; version entries — must.
                if section.version.is_empty() {
                    assert!(
                        entry.since.is_none(),
                        "{}: у записи «Основ» не должно быть версии",
                        entry.title
                    );
                } else {
                    assert_eq!(
                        entry.since.as_deref(),
                        Some(section.version.as_str()),
                        "{}: since должна совпадать с версией раздела",
                        entry.title
                    );
                }
            }
        }
    }

    #[test]
    fn since_versions_exist_in_changelog() {
        let known: Vec<String> = crate::changelog::changelog()
            .into_iter()
            .map(|e| e.version)
            .collect();
        for section in guide() {
            for entry in &section.entries {
                if let Some(since) = &entry.since {
                    assert!(
                        known.contains(since),
                        "{}: версии {} нет в changelog",
                        entry.title,
                        since
                    );
                }
            }
        }
    }
}




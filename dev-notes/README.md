# dev-notes — как устроен remoteplayer-mpvwatcher

Маленький сервис, который удобно читать целиком: одиннадцать файлов, около 2000
строк вместе с тестами. Общие темы Rust (владение, async, ошибки, стиль кода)
подробно разобраны в каталоге `dev-notes/` репозитория
[remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice);
здесь — только то, что специфично для наблюдателя.

| Файл | О чём |
|---|---|
| [01-how-it-works.md](01-how-it-works.md) | трекер на событиях mpv, файл и режим «только для чтения», сокет, systemd |
| [02-rust-in-this-code.md](02-rust-in-this-code.md) | приёмы Rust на примерах этого кода |
| [03-testing-without-pi.md](03-testing-without-pi.md) | как проверить без Raspberry Pi |

Порядок чтения кода: `src/main.rs` (запуск и остановка) → `src/tracker.rs`
(следит за mpv, пишет файл) → `src/snapshot.rs` (что снимаем) →
`src/recorder.rs` (правило «не затирать») → `src/shared.rs` (что видят
клиенты) → `src/output/` (файл и сокет) → `src/sampler.rs` (`get`) →
`src/mpv_connection.rs` (как разговариваем с mpv).

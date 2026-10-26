# dev-notes — как устроен remoteplayer-mpvwatcher

Маленький сервис, который удобно читать целиком: семь файлов, около тысячи
строк вместе с тестами. Общие темы Rust (владение, async, ошибки, стиль кода)
подробно разобраны в каталоге `dev-notes/` репозитория
[remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice);
здесь — только то, что специфично для наблюдателя.

| Файл | О чём |
|---|---|
| [01-how-it-works.md](01-how-it-works.md) | цикл опроса, правило «не затирать снимок», выходы, systemd |
| [02-rust-in-this-code.md](02-rust-in-this-code.md) | приёмы Rust на примерах этого кода |
| [03-testing-without-pi.md](03-testing-without-pi.md) | как проверить без Raspberry Pi |

Порядок чтения кода: `src/main.rs` (цикл и `Watcher`) → `src/snapshot.rs`
(что снимаем) → `src/recorder.rs` (что публикуем) → `src/output/` (куда) →
`src/mpv_connection.rs` (как разговариваем с mpv).

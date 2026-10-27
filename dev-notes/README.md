# dev-notes — как устроен remoteplayer-mpvwatcher

Маленький сервис, который удобно читать целиком: восемь файлов, около 1300
строк вместе с тестами. Общие темы Rust (владение, async, ошибки, стиль кода)
подробно разобраны в каталоге `dev-notes/` репозитория
[remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice);
здесь — только то, что специфично для наблюдателя.

| Файл | О чём |
|---|---|
| [01-how-it-works.md](01-how-it-works.md) | файл и сокет, общий опрос mpv, правило «не затирать снимок», systemd |
| [02-rust-in-this-code.md](02-rust-in-this-code.md) | приёмы Rust на примерах этого кода |
| [03-testing-without-pi.md](03-testing-without-pi.md) | как проверить без Raspberry Pi |

Порядок чтения кода: `src/main.rs` (таймеры и `FileRecorder`) →
`src/sampler.rs` (кто и когда спрашивает mpv) → `src/snapshot.rs` (что
снимаем) → `src/recorder.rs` (что пишем в файл) → `src/output/` (файл и
сокет) → `src/mpv_connection.rs` (как разговариваем с mpv).

# 03. Проверка без Raspberry Pi

У наблюдателя нет ничего специфичного для Pi: он общается только с IPC-сокетом
mpv. Поэтому «заглушка» здесь — обычный mpv на рабочем столе, а пути к
файлу и сокету в `config/dev.toml` указывают в `./dev-data` (он в `.gitignore`).

## Вручную

```sh
mkdir -p dev-data
mpv --idle=yes --force-window=yes --input-ipc-server=dev-data/mpv.sock &
RUST_LOG=debug cargo run -- --config config/dev.toml
```

Что стоит посмотреть:

1. Перетащить файл в окно mpv — через ≤7 секунд появится `dev-data/state.json`,
   а `socat - UNIX-CONNECT:dev-data/state.sock` покажет строку состояния.
2. Пауза → `sequence` перестаёт расти: состояние не меняется, и ничего не
   пишется.
3. Закрыть mpv и запустить новый (пустой) → в логе
   `mpv restarted and is still empty; keeping the previous snapshot`, а файл
   остаётся прежним.
4. В новом mpv открыть файл → публикуется состояние с новым `pid`.
5. Ctrl+C → `state.sock` исчезает.

## Вместе с REST-сервисом

В dev-конфиге REST-сервиса mpv запускает сам сервис (его сокет —
`../remoteplayer-restservice/dev-data/run/mpv.sock`). Если указать этот путь
в `mpv-socket` наблюдателя, а в `restore.watcher-state-file` REST-сервиса —
файл наблюдателя, можно проверить восстановление после падения: `kill -9`
процесса mpv. Супервизор REST-сервиса через 3 секунды поднимет mpv, сервис
увидит новый pid и восстановит воспроизведение по снимку.

## Юнит-тесты

`cargo test` — без mpv. В `src/mpv_connection.rs` есть `serve_fake_mpv`:
маленький сервер на `UnixListener`, который отвечает на `get_property` из
словаря, а перед каждым ответом шлёт постороннее событие, чтобы проверить,
что клиент его пропускает. Его же используют тесты `snapshot.rs`.
Остальное — чистая логика (`recorder.rs`) и временные каталоги
(`tempfile`) для файла и сокета.

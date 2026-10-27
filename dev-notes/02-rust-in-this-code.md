# 02. Rust на примерах этого кода

## Последовательный клиент вместо актора

В REST-сервисе соединением с mpv владеет отдельная задача-актор, потому что
запросы приходят параллельно от многих HTTP-обработчиков, а ещё через то же
соединение идут события. Здесь опрос — всегда целый снимок подряд, без
событий, поэтому `MpvConnection` — обычная структура с методами `&mut self`:

```rust
pub async fn get_property_optional<T: DeserializeOwned>(&mut self, property: &str) -> Result<Option<T>, MpvError>
```

`&mut self` — это гарантия компилятора, что два запроса одновременно по
одному соединению не уйдут: два изменяемых заимствования сразу невозможны.
В Java для того же понадобился бы `synchronized` или дисциплина.

`T: DeserializeOwned` — обобщённый тип с ограничением, «любой тип, который
serde умеет собрать из JSON»: `get_property_optional::<f64>("volume")`,
`get_property_optional::<Vec<MpvPlaylistEntry>>("playlist")`. Тип результата
выводится из аннотации переменной, в которую он присваивается.

## Однопоточный runtime

```rust
#[tokio::main(flavor = "current_thread")]
```

Весь асинхронный код — таймеры, сервер сокета, клиенты — работает в одном
потоке и переключается на `.await`. Это event loop, как в Node.js. Блокировать
его нельзя, поэтому запись файла с `fsync` вынесена в `spawn_blocking`.

## Общий ресурс: `Arc<Sampler>` + `tokio::sync::Mutex`

Соединением с mpv пользуются двое: таймер файла и задачи клиентов сокета.
Владелец у значения может быть только один, поэтому `Sampler` живёт в `Arc`
(разделяемое владение со счётчиком ссылок, как `std::shared_ptr`), а каждая
задача получает свою копию указателя:

```rust
let sampler: Arc<Sampler> = Arc::new(Sampler::new(...));
SocketServer::start(path, Arc::clone(&sampler))?;          // для клиентов сокета
FileRecorder { sampler: Arc::clone(&sampler), ... }         // для таймера
```

`Arc` даёт только чтение (`&Sampler`). Изменяемое состояние — соединение и
последний снимок — лежит внутри, под мьютексом:

```rust
pub struct Sampler {
    state: Mutex<SamplerState>,   // tokio::sync::Mutex
}

pub async fn capture_live(&self) -> Result<TimedCapture, MpvError> {
    let mut state: MutexGuard<'_, SamplerState> = self.state.lock().await;
    self.poll(&mut state).await
}
```

Это «внутренняя изменяемость» (interior mutability): метод принимает `&self`,
но меняет состояние, потому что доступ сериализован мьютексом. Компилятор
не даст поменять `SamplerState` в обход `lock()`.

Почему мьютекс из tokio, а не из std: он удерживается через `.await` (весь
опрос — около 15 запросов к mpv). Стандартный мьютекс на это время
заблокировал бы поток, а поток здесь всего один. Асинхронный мьютекс
вместо этого приостанавливает только ждущую задачу.

## `Drop` для уборки

```rust
impl Drop for SocketServer {
    fn drop(&mut self) { let _ignored: std::io::Result<()> = std::fs::remove_file(&self.path); }
}
```

При штатной остановке (SIGTERM от systemd) `run` возвращается, переменная
`_socket_server` выходит из области видимости, `SocketServer` уничтожается, и
файл сокета исчезает. Явного «close» в коде нет: уборка привязана к времени
жизни объекта, как в C++. Имя с подчёркиванием важно: `_socket_server` живёт
до конца функции, а голое `_` уничтожило бы сервер сразу.

## `Option` вместо флагов «включено»

```rust
let mut file_timer: Option<Interval> = file_recorder.as_ref().map(|_recorder: &FileRecorder| repeating(interval));
```

Выключенный выход — это не `bool enabled` рядом с полями, которые тогда
лишь «формально» заполнены, а `None`: без таймера нет и самого таймера.
Чтобы `select!` мог ждать необязательный таймер, есть функция `next_tick`:
при `None` она ждёт вечно (`std::future::pending`), и эта ветка просто
никогда не срабатывает.

## Чистая логика отдельно от ввода-вывода

`Recorder::consider(&mut self, capture: Capture, now_unix_ms: u64) -> Decision`
не обращается ни к сети, ни к диску, ни даже к часам: время передаётся
параметром (это момент снимка из `TimedCapture`). Поэтому логика тестируется простыми юнит-тестами без моков. Тот же
приём в REST-сервисе — `PlayerSnapshot::restore_commands`.

`capture` передаётся по значению, а не по ссылке: плейлист (вектор строк)
переезжает в новое состояние без копирования. Вызывающему код после вызова
`capture` больше не нужен, и сигнатура это фиксирует.

## enum как результат решения

```rust
pub enum Decision {
    Publish(SavedPlayerState),
    Unchanged,
    HoldPrevious,
}
```

Вместо `boolean shouldWrite` плюс отдельного поля с данными — один тип, в
котором данные есть только у того варианта, которому они нужны. Перепутать
«пишем, но забыли данные» невозможно.

## `let-else`

```rust
let Some(line): Option<String> = self.lines.next_line().await? else {
    return Err(MpvError::Closed);
};
```

«Разбери значение по образцу или выйди». Это ранний выход без лишнего
уровня вложенности: аналог `if (line == null) throw ...;` с последующим
использованием `line`, но компилятор знает, что после этой строки `line`
точно есть.

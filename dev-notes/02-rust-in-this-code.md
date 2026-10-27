# 02. Rust на примерах этого кода

## Последовательный клиент вместо актора

В REST-сервисе соединением с mpv владеет отдельная задача-актор: запросы
приходят параллельно от многих HTTP-обработчиков, и через то же соединение
идут события. Здесь проще: на одном соединении — только события (трекер), на
другом — только запросы по одному (`get`). Поэтому `MpvConnection` — обычная
структура с методами `&mut self`:

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

Весь асинхронный код — трекер, сервер сокета, клиенты — работает в одном
потоке и переключается на `.await`. Это event loop, как в Node.js. Блокировать
его нельзя, поэтому запись файла с `fsync` вынесена в `spawn_blocking`.

## Два мьютекса: из tokio и из std

**`Sampler`** держит соединение для `get` под `tokio::sync::Mutex`:

```rust
pub struct Sampler {
    connection: Mutex<Option<MpvConnection>>,   // tokio::sync::Mutex
}

pub async fn capture_live(&self) -> Result<TimedCapture, MpvError> {
    let mut guard: MutexGuard<'_, Option<MpvConnection>> = self.connection.lock().await;
    ...
}
```

Мьютекс удерживается через `.await` — весь опрос, около 15 запросов к mpv. Два
одновременных `get` встанут в очередь и не перемешаются в одном соединении.
Стандартный мьютекс на это время заблокировал бы единственный поток;
асинхронный приостанавливает только ждущую задачу.

**`SharedState`** (снимок `last`, статус) — под `std::sync::Mutex`. Каждая
критическая секция — короткое копирование внутрь или наружу, `.await`
внутри нет, поэтому лёгкий синхронный мьютекс здесь правильнее.

Правило простое: держишь блокировку через `.await` — tokio, нет — std.

В обоих случаях метод принимает `&self`, но меняет состояние: это
«внутренняя изменяемость» (interior mutability). Компилятор не даст добраться
до данных в обход `lock()`. Сами объекты разделены между задачами через `Arc`
(как `std::shared_ptr`): трекер, сервер сокета и каждый клиент получают свою
копию указателя.

## Остановка задачи по сигналу: `watch`-канал

Трекер работает в отдельной задаче (`tokio::spawn`). Прервать его «снаружи»,
просто уничтожив future, нельзя: он должен успеть записать финальный снимок.
Поэтому главный цикл посылает сигнал через `watch::channel(false)`, а трекер
ждёт его в своём `select!` рядом с событиями mpv:

```rust
let wakeup: Wakeup = tokio::select! {
    event = connection.next_event() => Wakeup::Event(event),
    () = sleep_until(due) => Wakeup::SnapshotDue,
    _ = shutdown.changed() => Wakeup::Shutdown,
};
```

Получив `Shutdown`, трекер делает отложенный снимок, пишет файл и
возвращается. `main` ждёт `JoinHandle` задачи не дольше 5 секунд
(`tokio::time::timeout`). Это аналог `CancellationToken` из .NET, только
явный и без исключений.

## `Drop` для уборки

```rust
impl Drop for SocketServer {
    fn drop(&mut self) { let _ignored: std::io::Result<()> = std::fs::remove_file(&self.path); }
}
```

При штатной остановке (SIGTERM от systemd) `run` дожидается трекера и
возвращается, переменная `_socket_server` выходит из области видимости, `SocketServer` уничтожается, и
файл сокета исчезает. Явного «close» в коде нет: уборка привязана к времени
жизни объекта, как в C++. Имя с подчёркиванием важно: `_socket_server` живёт
до конца функции, а голое `_` уничтожило бы сервер сразу.

## `Option` и enum вместо флагов «включено»

```rust
pub state_file: Option<PathBuf>,                 // TrackerConfig: None — файла нет
let mut watchdog_timer: Option<Interval> = ...;  // None — systemd не просил watchdog

pub enum StateFileMode {
    NotConfigured,
    ReadOnly { path: PathBuf, reason: String },
    ReadWrite { path: PathBuf, volatile: bool },
}
```

Выключенная функция — это не `bool enabled` рядом с полями, заполненными
«для формы», а отсутствие данных. У `ReadOnly` есть `reason`, но нет
`volatile`; у `ReadWrite` — наоборот. Невозможное сочетание («только для
чтения, но в RAM») нельзя даже записать.

Чтобы `select!` мог ждать необязательный таймер, есть функции `next_tick` и
`sleep_until`: при `None` они ждут вечно (`std::future::pending`), и эта ветка
просто никогда не срабатывает.

## Чистая логика отдельно от ввода-вывода

`Recorder::consider(&mut self, capture: Capture, now_unix_ms: u64) -> Decision`
не обращается ни к сети, ни к диску, ни даже к часам: время передаётся
параметром. Поэтому логика тестируется простыми юнит-тестами без моков. Тот же
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

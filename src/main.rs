use actix::prelude::*; // Для акторов WebSocket
use actix_web::{get, web, App, HttpServer, Responder, HttpResponse, Error as ActixWebError, HttpRequest};
use actix_web_actors::ws; // Для WebSocket акторов
use actix_files::Files;

use teloxide::prelude::*;
use teloxide::RequestError;
use teloxide::utils::command::BotCommands;
use teloxide::types::{KeyboardButton, KeyboardMarkup, Message};
use teloxide::dispatching::UpdateHandler;
use teloxide::dptree;
use teloxide::Bot;

use std::env;
use std::error::Error as StdError; // Переименовал, чтобы не путать с ActixWebError
use std::fs;
use std::collections::HashMap;
use std::sync::Arc;
use std::fmt::Display;
use std::time::{Duration, Instant}; // Для heartbeat WebSocket

use dotenv::dotenv;
use chrono::{Utc, DateTime};
use chrono_tz::Tz;
use reqwest::Client;
use serde::Deserialize;
use regex::Regex;
use once_cell::sync::Lazy;
use env_logger;
use toml;

// +++ Структуры ---
#[derive(Deserialize, Debug, Clone)] struct LocaleStrings { 
    greeting: String, status_check: String, status_check_command: String, unknown_command: String,
    fallback_reply: String, error_generic: String, weather_request_city: String,
    weather_fetch_error: String, weather_response: String, time_request_city: String,
    time_fetch_error: String, time_response: String, time_zone_not_found: String,
    time_zone_parse_error: String, time_server_response: String, time_city_suggestion: String,
    about_text: String, help_extra: String,
}
#[derive(Deserialize, Debug, Clone)] struct CityTimezones { cities: HashMap<String, String>, }
// AppState будет передаваться через web::Data для Actix и через dptree для Teloxide
#[derive(Clone)] struct AppState { client: Client, locale: Arc<LocaleStrings>, city_tz_map: Arc<CityTimezones> }
#[derive(Deserialize, Debug)] struct WeatherResponse { weather: Vec<WeatherInfo>, main: MainInfo, name: String, }
#[derive(Deserialize, Debug)] struct WeatherInfo { description: String, }
#[derive(Deserialize, Debug)] struct MainInfo { temp: f32, }

// +++ Переменные окружения и константы ---
static TELOXIDE_TOKEN: Lazy<String> = Lazy::new(|| env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN must be set"));
static WEATHER_API_KEY: Lazy<String> = Lazy::new(|| env::var("WEATHER_API_KEY").expect("WEATHER_API_KEY must be set"));
static WEB_PORT: Lazy<u16> = Lazy::new(|| env::var("WEB_PORT").unwrap_or("6001".to_string()).parse().expect("WEB_PORT must be a number"));
static DEFAULT_LANG: Lazy<String> = Lazy::new(|| env::var("DEFAULT_LANG").unwrap_or("ru".to_string()));

// +++ WebSocket Константы ---
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);


// +++ Регулярные выражения ---
static WEATHER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:какая|какую|скажи|подскажи|покажи|weather|tell|show)?\s*(?:погода|погоду)\s*(?:в|городе|для|in|for)?\s*(.+)").unwrap());
static TIME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:который|сколько|скажи|подскажи|покажи|what|tell|show)?\s*(?:время|час|time)\s*(?:в|городе|для|in|for)?\s*(.+)").unwrap());
static TIME_GENERAL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:который|сколько|скажи|подскажи|покажи|what|tell|show)?\s*(?:время|час|time)\s*$").unwrap());

// +++ Функции загрузки данных ---
fn load_locale(lang: &str) -> Result<LocaleStrings, Box<dyn StdError>> {
    let path = format!("locales/{}.toml", lang);
    log::info!("Загрузка локали из: {}", path);
    let content = fs::read_to_string(&path)?;
    let strings: LocaleStrings = toml::from_str(&content)?;
    Ok(strings)
}
fn load_city_timezones() -> Result<CityTimezones, Box<dyn StdError>> {
    let path = "data/cities_tz.toml";
    log::info!("Загрузка часовых поясов из: {}", path);
    let content = fs::read_to_string(path)?;
    let data: CityTimezones = toml::from_str(&content)?;
    Ok(data)
}

// +++ Вспомогательная функция форматирования ---
fn format_locale(format_string: &str, args: &[&dyn Display]) -> String {
    let mut result = format_string.to_string();
    for arg in args {
        if let Some(idx) = result.find("{}") {
             result.replace_range(idx..idx + 2, &arg.to_string());
        } else { break; }
    }
    result
}

// +++ Функции-обработчики для веб-сервера ---
#[get("/")] async fn index() -> impl Responder {  HttpResponse::Ok().body("<html><body><h1>Привет от Rust Ботa!</h1><p>Web интерфейс активен. Посетите <a href=\"/static/chat.html\">веб-чат</a> или <a href=\"/static/\">статическую страницу</a>.</p></body></html>") }
async fn status_handler() -> impl Responder {  HttpResponse::Ok().json(serde_json::json!({"telegram_status": "OK", "web_status": "OK"})) }


// +++ WebSocket Актор ---

struct WsChatSession {
    hb: Instant,           // Время последнего пинга (heartbeat)
    app_state: Arc<AppState>, // Доступ к общему состоянию
}

impl WsChatSession {
    // Метод для проверки heartbeat
    fn hb(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            // Проверяем таймаут клиента
            if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
                log::info!("WebSocket Client heartbeat failed, disconnecting!");
                ctx.stop();
                return;
            }
            // Отправляем пинг клиенту
            ctx.ping(b"");
        });
    }
}

impl Actor for WsChatSession {
    type Context = ws::WebsocketContext<Self>;

    //Метод вызывается при старте актора (установке соединения)
    fn started(&mut self, ctx: &mut Self::Context) {
        log::info!("WebSocket соединение установлено");
        self.hb(ctx); //Запускаем проверку heartbeat
    }

     fn stopping(&mut self, _: &mut Self::Context) -> Running {
        log::info!("WebSocket соединение закрывается");
        Running::Stop
    }
}

// Обработчик входящих сообщений WebSocket
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now(); // Обновляем вквмя heartbeat при пинге
                ctx.pong(&msg); // Отправляем понг обратно
            }
            Ok(ws::Message::Pong(_)) => {
                self.hb = Instant::now(); // Обновляем время heartbeat при понге
            }
            Ok(ws::Message::Text(text)) => {
                log::debug!("WebSocket получено сообщение: {}", text);
                self.hb = Instant::now(); // Обновляем время heartbeat

                // Клонируем Arc состояния для передачи в асинхронную задачу
                let state = self.app_state.clone();
                let user_input = text.to_string(); // Клонируем текст

                // Запускаем асинхронную задачу для обработки запроса что бы не блокировать актор WebSocket
                ctx.spawn(
                    async move {
                        // Вызываем общую логику генерации ответа
                        let response = generate_response(&user_input, &state).await;
                        response // Возвращаем ответ из задачи
                    }
                    .into_actor(self) // Преобразуем Future в ActorFuture
                    .then(|response, _act, ctx| {
                         // Этот блок выполнится, когда generate_response завершится
                         log::debug!("WebSocket отправка ответа: {}", response);
                         ctx.text(response); // Отправляем ответ клиенту
                         fut::ready(()) // Завершаем обработку
                    }),
                );
            }
            Ok(ws::Message::Binary(bin)) => {
                log::warn!("Получено бинарное WebSocket сообщение (игнор)");
                ctx.binary(bin); // Можно эхом вернуть, но смысл
            }
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => ctx.stop(), // Ошика - останавливаем актор
        }
    }
}

// +++ Обработчик HTTP запроса для старта WebSocket ---
async fn chat_ws(
    req: HttpRequest,
    stream: web::Payload,
    // Получаем AppState через web::Data
    app_state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ActixWebError> {
    // Создаем и запускаем актор WsChatSession
    ws::start(
        WsChatSession {
            hb: Instant::now(),
            // Клонируем Arc для передачи в актор
            app_state: app_state.get_ref().clone(),
        },
        &req,
        stream,
    )
}

// +++ Команды Telegram ---
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Доступные команды:")]
enum Command {
    #[command(description = "Показать это сообщение.")] Help,
    #[command(description = "Показать информацию о боте.")] About,
    #[command(description = "Проверить статус систем.")] Status,
    #[command(description = "Узнать погоду в Москве.")] WeatherMoscow,
    #[command(description = "Узнать время в Лондоне.")] TimeLondon,
}

// +++ ОБЩАЯ ЛОГИКА ГЕНЕРАЦИИ ОТВЕТА ---
async fn generate_response(input_text: &str, state: &AppState) -> String {
    let locale = &state.locale;
    let client = &state.client;
    let city_tz_map = &state.city_tz_map.cities;
    let text_lc = input_text.trim().to_lowercase();

    log::debug!("generate_response для текста: '{}'", input_text);

    // +++ Анализ реплики ---
    if text_lc == "привет" || text_lc == "здравствуй" || text_lc == "hello" || text_lc == "hi" {
        locale.greeting.clone()
    } else if text_lc == "как твои дела?" || text_lc == "как дела" {
        locale.status_check.clone()
    }
    // +++ Погодка, главное не забыть API в .env, иначе всё поломается ---
    else if let Some(caps) = WEATHER_RE.captures(input_text) {
        if let Some(city_match) = caps.get(1) {
            let city = city_match.as_str().trim();
            if !city.is_empty() {
                match get_weather(client, city).await {
                    Ok((resolved_city, temp, description)) => {
                        
                         let temp_str = format!("{:.1}", temp); //
                         format_locale(&locale.weather_response, &[&resolved_city, &temp_str, &description])
                         
                    }
                    Err(e) => {
                         format_locale(&locale.weather_fetch_error, &[&city, &e])
                    }
                }
            } else {
                 locale.weather_request_city.clone()
            }
        } else {
             locale.fallback_reply.clone()
        }
    }
    // +++ Время ---
    else if let Some(caps) = TIME_RE.captures(input_text) {
        if let Some(city_match) = caps.get(1) {
             let city_input = city_match.as_str().trim();
             if !city_input.is_empty() {
                 match get_time_for_city(city_input, city_tz_map) {
                     Ok(time_str) => {
                         format_locale(&locale.time_response, &[&city_input, &time_str])
                    }
                     Err(e) => {
                         if e == "Не найден часовой пояс" {
                              format_locale(&locale.time_zone_not_found, &[&city_input])
                         } else {
                              format_locale(&locale.time_fetch_error, &[&city_input, &e])
                         }
                    }
                 }
             } else {
                  locale.time_request_city.clone()
             }
        } else {
            locale.fallback_reply.clone() // если нет города
        }
    }
    // +++ Время (общее) ---
    else if TIME_GENERAL_RE.is_match(input_text) {
        let now: DateTime<Utc> = Utc::now();
        let now_str = now.format("%H:%M:%S (%Y-%m-%d %Z)").to_string();
        // Отправляем два сообщения в TelegramЮ а одно в веб-чат
        // Для веб-чата
        format!("{}\n{}",
            format_locale(&locale.time_server_response, &[&now_str]),
            locale.time_city_suggestion
        )
    }
    // +++ Команды (Телеграм обработает их сам, здесь для веб) ---
    // Простая проверка на /команду (можно улучшить, но мне лень)
    else if input_text.starts_with('/') {
        // Здесь можно добавить логику разбора команд, похожую на teloxide,
        // но пока просто возвращаем заглушку или help
        format!("Команды для веб-чата пока не реализованы. Попробуйте в Telegram.\nИли используйте: 'Погода в ...', 'Время в ...'")
    }
    // +++ Ответ по умолчанию ---
    else {
        format_locale(&locale.unknown_command, &[&input_text])
    }
}

// +++ Обработчик Teloxide ---
async fn telegram_message_handler( bot: Bot, msg: Message, state: Arc<AppState> ) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let q_text = msg.text().unwrap_or_default();

    // Обработка команд через BotCommands
    match BotCommands::parse(q_text, bot.get_me().await?.username()) {
         Ok(command) => match command {
             Command::Help => {
                 let help_text = format!("{}\n\n{}", Command::descriptions().to_string(), state.locale.help_extra);
                 bot.send_message(chat_id, help_text).reply_markup(make_keyboard()).await?;
             }
             Command::About => {
                 let port_str = WEB_PORT.to_string();
                 let response_text = format_locale(&state.locale.about_text, &[&port_str]);
                 bot.send_message(chat_id, response_text).reply_markup(make_keyboard()).await?;
             }
              Command::Status => {
                 bot.send_message(chat_id, &state.locale.status_check_command).reply_markup(make_keyboard()).await?;
             }
              Command::WeatherMoscow => {
                  match get_weather(&state.client, "Москва").await {
                     Ok((resolved_city, temp, description)) => {
                          let response_text = format_locale(&state.locale.weather_response, &[&resolved_city, &temp, &description]);
                          bot.send_message(chat_id, response_text).await?;
                     }
                     Err(e) => {
                          let response_text = format_locale(&state.locale.weather_fetch_error, &[&"Москва", &e]);
                          bot.send_message(chat_id, response_text).await?;
                     }
                  };
              }
              Command::TimeLondon => {
                   match get_time_for_city("Лондон", &state.city_tz_map.cities) {
                      Ok(time_str) => {
                           let response_text = format_locale(&state.locale.time_response, &[&"Лондон", &time_str]);
                           bot.send_message(chat_id, response_text).await?;
                      }
                      Err(e) => {
                           let response_text = format_locale(&state.locale.time_fetch_error, &[&"Лондон", &e]);
                           bot.send_message(chat_id, response_text).await?;
                      }
                  };
              }
        },
        Err(_) => {
            // Если это не команда /*** , используем общую логику
            let response_text = generate_response(q_text, &state).await;
            // Отправляем ответ
            bot.send_message(chat_id, response_text).reply_markup(make_keyboard()).await?;
        }
    }

    Ok(())
}

// +++ Кнопочки ---
fn make_keyboard() -> KeyboardMarkup {
    let keyboard = vec![
        vec![KeyboardButton::new("/about"), KeyboardButton::new("/status")],
        vec![KeyboardButton::new("/WeatherMoscow"), KeyboardButton::new("/TimeLondon")],
        vec![KeyboardButton::new("/help")],
    ];
    KeyboardMarkup::new(keyboard).resize_keyboard()
}

// +++ Вспомогательные функции (Погода, Время) ---
async fn get_weather(client: &Client, city: &str) -> Result<(String, f32, String), String> { 
    let url = format!( "https://api.openweathermap.org/data/2.5/weather?q={}&appid={}&units=metric&lang=ru", city, *WEATHER_API_KEY );
    log::debug!("Requesting weather URL: {}", url);
    let resp = client.get(&url).send().await.map_err(|e| format!("Ошибка сети: {}", e))?;
    let response_status = resp.status();
    log::debug!("Weather API response status: {}", response_status);
    if response_status.is_success() {
        match resp.json::<WeatherResponse>().await {
            Ok(data) => {
                 log::debug!("Weather API response data: {:?}", data);
                 let description = data.weather.first().map_or("нет данных", |w| &w.description).to_string();
                 Ok((data.name, data.main.temp, description))
            },
            Err(e) => { log::error!("Weather API JSON parse error: {}", e); Err(format!("Ошибка обработки ответа API погоды")) },
        }
    } else {
         let error_body = resp.text().await.unwrap_or_else(|_| "не удалось прочитать тело ошибки".to_string());
         log::error!("Weather API error: Status {}, Body: {}", response_status, error_body);
         if error_body.contains("city not found") { Err("Город не найден".to_string()) }
         else { Err(format!("Ошибка API погоды ({})", response_status)) }
    }
}
fn get_time_for_city(city_input: &str, city_tz_map: &HashMap<String, String>) -> Result<String, String> { 
    let city_lowercase = city_input.trim().to_lowercase();
    if let Some(timezone_str) = city_tz_map.get(&city_lowercase) {
        match timezone_str.parse::<Tz>() {
            Ok(tz) => {
                let now: DateTime<Tz> = Utc::now().with_timezone(&tz);
                Ok(now.format("%H:%M:%S (%Y-%m-%d %Z)").to_string())
            }
			// Если API умер
            Err(_) => { log::error!("Не удалось распознать часовой пояс из карты: '{}' для города '{}'", timezone_str, city_input); Err(format!("Внутренняя ошибка: неверный пояс '{}' в карте", timezone_str)) }
        }
    } else { Err("Не найден часовой пояс".to_string()) }
}

// +++ Schema для dptree ---
type HandlerSchema = UpdateHandler<RequestError>;

fn telegram_schema() -> HandlerSchema {
    dptree::entry()
         // Явно обрабатываем команды через Command::parse()
        .branch(Update::filter_message().filter_command::<Command>().endpoint(telegram_message_handler))
        // Остальные сообщения обрабатываем через generate_response
        .branch(Update::filter_message().endpoint(telegram_message_handler))
}

// +++ Основная функция ---
#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError>> { // Используем StdError
    dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("Загрузка конфигурации...");
    let lang = DEFAULT_LANG.clone();
    let locale = Arc::new(load_locale(&lang)?);
    let city_timezones = Arc::new(load_city_timezones()?);

    log::info!("Запуск бота...");
    let client = Client::new(); // Создаем HTTP клиент один раз.
    let bot = Bot::new(TELOXIDE_TOKEN.clone());

    // Создаем общее состояние для Actix и Teloxide
    let shared_state = Arc::new(AppState {
        client: client.clone(), // Клонируем для состояния
        locale: locale.clone(),
        city_tz_map: city_timezones.clone(),
    });

    // Оборачиваем состояние для Actix
    let actix_state = web::Data::new(shared_state.clone());

    // +++ Настройка диспетчера Teloxide ---
    log::info!("Настройка диспетчера Telegram...");
    let mut dispatcher = Dispatcher::builder(bot, telegram_schema()) // Используем telegram_schema
        .dependencies(dptree::deps![shared_state.clone()]) // Передаем Arc<AppState>
        .enable_ctrlc_handler()
        .build();

    log::info!("Запуск диспетчера Telegram...");
    let run_telegram_bot = dispatcher.dispatch();

    // +++ Настройка веб-сервера Actix ---
    let web_port = *WEB_PORT;
    let run_web_server = HttpServer::new(move || { // Используем move
        App::new()
            .app_data(actix_state.clone()) // Передаем состояние в Actix приложение
            .service(index)
            .route("/status", web::get().to(status_handler))
             // Добавляем маршрут для WebSocket
            .route("/ws/", web::get().to(chat_ws))
            .service(Files::new("/static", "static").show_files_listing().index_file("index.html")) // Статика должна быть последней
    })
    .bind(("0.0.0.0", web_port))?
    .run();

    log::info!("Веб-сервер запущен на http://0.0.0.0:{}", web_port);

    // +++ Запуск обоих служб ---
    let (web_result, _) = tokio::join!(run_web_server, run_telegram_bot);

    match web_result {
        Ok(()) => log::info!("Веб-сервер завершил работу штатно."),
        Err(e) => log::error!("Ошибка веб-сервера: {}", e),
    }
    log::info!("Telegram диспетчер завершил работу.");
    log::info!("Приложение завершило работу.");

    Ok(())
}

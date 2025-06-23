use std::collections::HashMap;
use std::time::Duration;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use sqlx::SqlitePool;
use thiserror::Error;
use lettre::{Message, SmtpTransport, Transport};
use lettre::transport::smtp::authentication::Credentials;
use tokio::sync::Mutex;
use once_cell::sync::Lazy;
use tokio::time::Instant;

#[derive(Debug, Error)]
enum AppError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("MQTT Error: {0}")]
    Mqtt(#[from] rumqttc::ClientError),
    #[error("Environment Error: {0}")]
    Env(#[from] std::env::VarError),
    #[error("Parse Error: {0}")]
    Parse(#[from] std::num::ParseIntError),
}




static LAST_SENT_TIMES: Lazy<Mutex<HashMap<String, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));
const COOLDOWN_DURATION: Duration = Duration::from_secs(600);

async fn maybe_send_email(subject: &str, body: &str, topic: &str) {
    let username = std::env::var("EMAIL_USERNAME").expect("email username not set");
    let password = std::env::var("EMAIL_PASSWORD").expect("email password not set");
    let recipient = std::env::var("EMAIL_RECIPIENT").expect("email recipient not set");

    let mut times = LAST_SENT_TIMES.lock().await;
    let now = Instant::now();
    if let Some(last_sent) = times.get(topic) {
        if now.duration_since(*last_sent) < COOLDOWN_DURATION {
            println!("Cooldown active f or topic: {}", topic);
            return;
        }
    }

    let email = Message::builder()
        .from(username.parse().unwrap())
        .to(recipient.parse().unwrap())
        .subject(subject)
        .body(body.to_string())
        .unwrap();

    let credentials = Credentials::new(username.clone(), password);
    let mailer = SmtpTransport::relay("smtp.gmail.com")
        .unwrap()
        .credentials(credentials)
        .build();

    match mailer.send(&email) {
        Ok(_) => {
            println!("Email sent for topic: {}", topic);
            times.insert(topic.to_string(), now);
        },
        Err(e) => eprintln!("Failed to send email: {:?}", e),
    }
}
#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is not set in .env file");
    let mqtt_host = std::env::var("MQTT_HOST").expect("MQTT_HOST is not set in .env file");
    let mqtt_port = std::env::var("MQTT_PORT").expect("MQTT_PORT is not set in .env file");

    let db_pool = SqlitePool::connect(&database_url).await.expect("Database connection failed");

    sqlx::migrate!().run(&db_pool).await.expect("Failed to run migrations");

    loop {
        if let Err(e) = start_mqtt_subscriber(&mqtt_host, &mqtt_port, db_pool.clone()).await {
            eprintln!("MQTT subscriber error: {}", e);
        }
    }
}

async fn start_mqtt_subscriber(
    host: &str,
    port: &str,
    db_pool: SqlitePool,
) -> Result<(), AppError> {
    let mut mqtt_options = MqttOptions::new("rust-mqtt-subscriber", host, port.parse()?);
    mqtt_options.set_keep_alive(Duration::from_secs(5));

    let (client, mut event_loop) = AsyncClient::new(mqtt_options, 10);
    client.subscribe("esp32/temperature", QoS::AtMostOnce).await?;
    client.subscribe("esp32/humidity", QoS::AtMostOnce).await?;
    client.subscribe("esp32/motion", QoS::AtMostOnce).await?;
    client.subscribe("esp32/contact", QoS::AtMostOnce).await?;
    println!("MQTT connected and subscribed to topics");

    while let Ok(event) = event_loop.poll().await {
        match event {
            Event::Incoming(Incoming::Publish(publish)) => {
                let topic = publish.topic;
                let payload = String::from_utf8_lossy(&publish.payload).to_string();

                println!("Received on {}: {}", topic, payload);

                match topic.as_str() {
                    "esp32/temperature" => {
                        let temp_value: i32 = payload.parse()?;
                        sqlx::query!(
                            "insert into temperature (value) values (?)", temp_value
                        ).execute(&db_pool).await?;
                    }
                    "esp32/humidity" => {
                        let humid_value: i32 = payload.parse()?;
                        sqlx::query!(
                            "insert into humidity (value) values (?)", humid_value
                        ).execute(&db_pool).await?;
                    }
                    "esp32/motion" => {
                        let motion_value: i32 = payload.parse()?;
                        sqlx::query!(
                            "insert into motion (value) values (?)", motion_value
                        ).execute(&db_pool).await?;
                        if motion_value == 1 {
                            maybe_send_email("Motion alert", "Motion was detected!", "esp32/motion").await;
                        }

                    }
                    "esp32/contact" => {
                        let contact_value: i32 = payload.parse()?;
                        sqlx::query!(
                            "insert into contact (value) values (?)", contact_value
                        ).execute(&db_pool).await?;
                        if contact_value == 1 {
                            maybe_send_email("Contact alert", "Contact sensor was detected!", "esp32/contact").await;
                        }
                    }
                    _ => println!("Unknown topic: {}", topic),
                }
            }
            _ => {}
        }
    }
    Ok(())
}
